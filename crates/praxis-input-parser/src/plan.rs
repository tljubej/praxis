//! Parser plans: the flat runtime representation of a parser AST.
//!
//! After validation and type synthesis, a [`ParserAst`] is lowered into a
//! [`ParserPlan`] — a flat arena of [`PlanNode`]s that the runtime interpreter
//! (`praxis-runtime::parser`) walks against the input buffer. The compiled plan
//! is registered in a process-wide arena and identified by a
//! [`PlanId`]; MIR passes that id as an `i64` immediate (no
//! pointer-as-immediate needed).
//!
//! Design constraints:
//! - **Flat and self-contained**: nodes reference children by **index** (no
//!   `Box`, no owned `String` on the hot path). Separators and template
//!   literals are interned into a parallel `&'static [&'static str]` slice so
//!   the runtime reads them without dereferencing Rust owned data.
//! - Record schemas for named-capture templates are built at **runtime** (the
//!   interpreter knows the field descriptors from the child plans' result
//!   types); the plan stores only field names as `&'static str`.
//!
//! **Not `#[repr(C)]`.** These are ordinary Rust enums and slices with the
//! default representation, and nothing here crosses an FFI boundary: the plan
//! is consumed by `praxis-runtime`, which is Rust and links against this crate,
//! and only the plan *id* is passed as a JIT immediate. If a plan ever does
//! need to be read by generated code, that is a real representation change
//! (explicit `#[repr(C)]`, no enums with payloads, no `&str` fat pointers) and
//! not something to assume from this comment.
//!
//! # Ownership
//!
//! Each [`CompiledPlan`] owns a `bumpalo` arena holding everything the plan's
//! `&'static` fields point into, so a plan is reclaimable at all; registration
//! is bounded and checked, so a long enough compile cannot wrap the index and
//! hand the runtime a *different* plan; and a [`PlanId`] is a `NonZeroU32`, so
//! "no plan" is not spelled `0` — it is not spellable at all.
//!
//! Reclamation has the same ordering obligation as the JIT generation arena:
//! record schemas the runtime builds for named-capture templates borrow their
//! field names from plan storage, so plans may only be retired once the heap
//! is drained. `praxis_runtime::retire_parser_plans` is the gate; see
//! [`retire_all_plans`].

use std::num::NonZeroU32;
use std::sync::Mutex;

use bumpalo::Bump;

use crate::ast::{AtomicKind, ParserAst, SkipPolicy, TemplatePart, WsPolicy};

// ===========================================================================
// The flat plan node arena.
// ===========================================================================

/// One node in the flattened parser plan. Index-based: children refer to other
/// nodes by their position in the `ParserPlan::nodes` slice.
#[derive(Debug)]
pub enum PlanNode {
    /// An atomic parser.
    Atomic { kind: AtomicKind },
    /// `lines(P)`.
    Lines { child: u32 },
    /// `sections(P)` (homogeneous).
    Sections { child: u32 },
    /// Named heterogeneous `sections(name: P, ..., tail: repeated(P))`.
    /// `fields` are the named arguments in source order, each contributing one
    /// record field and consuming
    /// [`SectionItemNode::sections_wanted`] sections; `repeated_tail` is the
    /// unbounded tail's `(name, child_index)`, if present, and it consumes
    /// every section the fields left.
    SectionsNamed {
        fields: &'static [SectionItemNode],
        repeated_tail: Option<(&'static str, u32)>,
        /// The result record's canonical field order (see [`FieldOrder`]).
        field_order: &'static [&'static str],
    },
    /// `block(item, ...)` (§7.5). Sequential parsers within one region;
    /// positional named-capture templates flatten their fields into the result
    /// record, named items contribute one field each.
    Block {
        items: &'static [BlockItemNode],
        /// The result record's canonical field order (see [`FieldOrder`]).
        field_order: &'static [&'static str],
    },
    /// `choice(Name: P, ...)` (§7.5). Try each case in source order; the
    /// first match wins and its value becomes the variant's payload. `cases`
    /// are `(name, child_index)` pairs.
    Choice {
        cases: &'static [(&'static str, u32)],
    },
    /// `optional(P)` (§7.5). Parse `P`; on success return Some(value)
    /// (Option tag 0), on failure consume nothing and return None (tag 1).
    Optional { child: u32 },
    /// `scan(P)` (§7.5). Slide a cursor; at each position try `P`; collect
    /// matches in source order, ignoring unmatched text.
    Scan { child: u32 },
    /// `one_of("LR")` (§7.5). `chars_index` is into `ParserPlan::literals`.
    OneOf { chars_index: u32 },
    /// `chars(P, skip:)` (§7.5). Apply a char-parser repeatedly.
    Characters { child: u32, skip: SkipPolicy },
    /// `matrix(P)` (§7.5, ADR-030). Whitespace-tokenized rectangular Grid.
    Matrix { child: u32 },
    /// Ragged `grid(P, ragged, fill:)` (§7.5). `fill_index` into literals.
    GridRagged { child: u32, fill_index: u32 },
    /// `csv(P)`.
    Csv { child: u32 },
    /// `ws(P)`.
    Ws { child: u32 },
    /// `sep(separator_index, P)`.
    Sep { separator_index: u32, child: u32 },
    /// `grid(P)`.
    Grid { child: u32 },
    /// A backtick template. `parts` are indices into [`ParserPlan::template_parts`].
    ///
    /// **Every template shape lowers to this**, multi-anonymous-capture tuples
    /// included; see [`TemplateShape`]. There is deliberately no `Tuple` node
    /// beside it — see that type's doc for why one cannot exist.
    Template {
        parts: &'static [TemplatePartNode],
        /// The result record's canonical field order (see [`FieldOrder`]), and
        /// **empty for the shapes that are not records** — a tuple's element
        /// order is its capture order and nothing reorders it, so there is no
        /// second opinion for this to carry.
        field_order: &'static [&'static str],
    },
}

/// One part of a template, in plan form.
#[derive(Debug)]
pub enum TemplatePartNode {
    /// A literal match.
    Literal { text: &'static str, ws: WsPolicy },
    /// A capture whose value comes from the child plan node. `field_index` is the
    /// position in the resulting record/tuple (None for single-capture scalars).
    Capture {
        child: u32,
        field_index: Option<u16>,
        name: Option<&'static str>,
    },
}

/// What a lowered template's parts add up to (§7.3).
///
/// §7.3: named captures produce an anonymous record; anonymous captures produce
/// a scalar when there is one and a tuple when there are several. A template
/// with no captures matches literally and produces `Unit`.
///
/// **Stated here, next to the parts it classifies, and asked rather than
/// re-derived** (ADR-092). The interpreter assembles a template's value
/// (`walk_template`) and tags a collection built from it
/// (`template_result_descriptor`) through this one function, because answering
/// the question separately is how the two drift: a tag that says `Unit` for the
/// tuple shape makes ``read lines(`{int},{int}`)`` print `[Unit, Unit]` and
/// compare unequal to an identical `Vec` built with `push`, while
/// `praxis check` types it `Vec[(Int, Int)]` throughout.
///
/// **There is no `PlanNode::Tuple`, and that is not an omission.** A variant
/// carrying only child indices cannot represent a multi-capture template,
/// because the template's separators are `TemplatePartNode::Literal`s between
/// the captures — `` `{int},{int}` `` would lose its comma. Widening it to hold
/// the literals makes it `PlanNode::Template` again. So the tuple shape is a
/// property of a `Template`'s parts, which is what this type reads, and the
/// state "a tuple node" is unnameable rather than merely unreachable.
///
/// `synthesize::template_type` answers the same question for the *type*, over
/// AST `TemplatePart`s rather than lowered ones. Keep the two in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateShape {
    /// No captures: the template matches literally and produces `Unit`.
    Unit,
    /// One anonymous capture: the result is that capture's value, and its type
    /// is the child parser's own result type. `child` is the child node index.
    Scalar { child: u32 },
    /// At least one named capture: an anonymous record, one field per capture.
    /// Mixing named and anonymous captures in one template is rejected before
    /// lowering (§7.3), so "any named" and "all named" are the same set here.
    Record,
    /// Two or more captures, none named: a tuple, one element per capture in
    /// source order.
    Tuple,
}

impl TemplateShape {
    /// Classify a lowered template's parts.
    pub fn of(parts: &[TemplatePartNode]) -> TemplateShape {
        let mut captures = 0usize;
        let mut any_named = false;
        let mut sole_anonymous: Option<u32> = None;
        for part in parts {
            if let TemplatePartNode::Capture { child, name, .. } = part {
                captures += 1;
                match name {
                    Some(_) => any_named = true,
                    None => sole_anonymous = Some(*child),
                }
            }
        }
        match (any_named, captures) {
            (true, _) => TemplateShape::Record,
            (false, 0) => TemplateShape::Unit,
            (false, 1) => match sole_anonymous {
                Some(child) => TemplateShape::Scalar { child },
                // Not reachable: one capture with none named *is* that one
                // anonymous capture. Answering `Unit` rather than panicking
                // keeps this total, which matters because the interpreter that
                // calls it runs under `extern "C"`, where a panic is undefined
                // behaviour.
                None => TemplateShape::Unit,
            },
            (false, _) => TemplateShape::Tuple,
        }
    }
}

/// One named argument of a heterogeneous `sections(...)` other than its
/// unbounded tail (§7.5), in plan form.
///
/// The count is a plain `u32` and not the [`crate::ast::RepeatCount`] newtype:
/// the invariant is discharged upstream, where the source span is still in hand
/// to report the violation against, and the plan is a flat `&'static` repr the
/// runtime reads without unwrapping anything.
#[derive(Debug)]
pub enum SectionItemNode {
    /// `name: P` — one section.
    One { name: &'static str, child: u32 },
    /// `name: repeated(P, N)` — exactly `count` consecutive sections, collected
    /// into one `Vec` field. Never zero.
    Counted {
        name: &'static str,
        child: u32,
        count: u32,
    },
}

impl SectionItemNode {
    /// The record field this item contributes.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            SectionItemNode::One { name, .. } | SectionItemNode::Counted { name, .. } => name,
        }
    }

    /// How many sections this item consumes — the number the runtime's section
    /// cursor advances by, and the number the shortfall check sums.
    #[must_use]
    pub fn sections_wanted(&self) -> usize {
        match self {
            SectionItemNode::One { .. } => 1,
            SectionItemNode::Counted { count, .. } => *count as usize,
        }
    }
}

/// One item of a `block(...)` (§7.5), in plan form.
#[derive(Debug)]
pub enum BlockItemNode {
    /// A positional parser. A named-capture template's fields flatten into the
    /// block record (the runtime reads the record's fields from the produced
    /// value); any other positional must have been named (validation rejects).
    Positional { child: u32 },
    /// A named item contributing one field.
    Named { name: &'static str, child: u32 },
}

/// A compiled parser plan: the node arena plus auxiliary interned data.
pub struct ParserPlan {
    /// The flat node arena, indexed by `PlanNode` child references.
    pub nodes: &'static [PlanNode],
    /// Template literal/capture parts, referenced by `PlanNode::Template`.
    pub template_parts: &'static [TemplatePartNode],
    /// Interned string literals (separators, template literals), so the runtime
    /// reads `&'static str` without touching Rust owned data.
    pub literals: &'static [&'static str],
    /// The root node index (entry point).
    pub root: u32,
}

impl std::fmt::Debug for ParserPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParserPlan")
            .field("nodes", &self.nodes)
            .field("template_parts_len", &self.template_parts.len())
            .field("literals", &self.literals)
            .field("root", &self.root)
            .finish()
    }
}

// ===========================================================================
// Lowering: ParserAst → &'static ParserPlan
// ===========================================================================

/// A builder that accumulates plan nodes into owned Vecs, then moves them into
/// the compiled plan's arena in [`PlanBuilder::finish`].
struct PlanBuilder<'a> {
    arena: &'a Bump,
    nodes: Vec<PlanNode>,
    template_parts: Vec<TemplatePartNode>,
    literals: Vec<&'static str>,
    order: &'a mut dyn FieldOrder,
}

impl<'a> PlanBuilder<'a> {
    fn new(arena: &'a Bump, order: &'a mut dyn FieldOrder) -> Self {
        PlanBuilder {
            arena,
            nodes: Vec::new(),
            template_parts: Vec::new(),
            literals: Vec::new(),
            order,
        }
    }

    /// The canonical order of an anonymous record shape's fields, allocated in
    /// this plan's arena so the runtime reads `&'static str`.
    ///
    /// `names` are the fields in the order this parser *writes* them, which is
    /// the order the value is assembled in; the answer is the order the value
    /// must be **laid out** in. The two differ only when some other spelling of
    /// the same shape got there first — see [`FieldOrder`].
    fn canonical_order(&mut self, names: &[&str]) -> &'static [&'static str] {
        let canonical = self.order.canonical(names);
        let entries: Vec<&'static str> = canonical.iter().map(|n| self.alloc_str(n)).collect();
        self.alloc_slice(entries)
    }

    /// Push a node and return its index.
    fn push_node(&mut self, node: PlanNode) -> u32 {
        let idx = self.nodes.len() as u32;
        self.nodes.push(node);
        idx
    }

    /// Intern a literal string, returning its index. The input `s` must already
    /// live in this plan's arena (see [`PlanBuilder::alloc_str`]).
    fn intern_literal(&mut self, s: &'static str) -> u32 {
        let idx = self.literals.len() as u32;
        self.literals.push(s);
        idx
    }

    /// Copy `s` into the plan's arena.
    fn alloc_str(&self, s: &str) -> &'static str {
        alloc_str(self.arena, s)
    }

    /// Move a `Vec<T>` into the plan's arena.
    fn alloc_slice<T>(&self, v: Vec<T>) -> &'static [T] {
        alloc_slice(self.arena, v)
    }

    /// Move the accumulated data into the arena and return the plan.
    fn finish(self, root: u32) -> &'static ParserPlan {
        let PlanBuilder {
            arena,
            nodes,
            template_parts,
            literals,
            order: _,
        } = self;
        let plan: &ParserPlan = arena.alloc(ParserPlan {
            nodes: alloc_slice(arena, nodes),
            template_parts: alloc_slice(arena, template_parts),
            literals: alloc_slice(arena, literals),
            root,
        });
        // SAFETY: see `alloc_str`.
        unsafe { &*(plan as *const ParserPlan) }
    }
}

/// Copy `s` into `arena`, erasing the lifetime to `'static`.
///
/// The `'static` is a lie the plan's own field types force: `PlanNode` and
/// `ParserPlan` declare `&'static str`. The truth is *arena* lifetime, and
/// [`CompiledPlan`] is what keeps the arena alive exactly as long as the plan.
/// This function and [`alloc_slice`] are the only two places the erasure
/// happens, and [`retire_all_plans`]'s safety contract is what discharges it.
fn alloc_str(arena: &Bump, s: &str) -> &'static str {
    let stored: &str = arena.alloc_str(s);
    // SAFETY: the bytes live in the arena the `CompiledPlan` owns alongside the
    // plan, and both are released together by `retire_all`.
    unsafe { &*(stored as *const str) }
}

/// Move a `Vec<T>` into `arena`. Same lifetime erasure as [`alloc_str`].
fn alloc_slice<T>(arena: &Bump, v: Vec<T>) -> &'static [T] {
    let stored: &[T] = arena.alloc_slice_fill_iter(v);
    // SAFETY: as `alloc_str`.
    unsafe { &*(stored as *const [T]) }
}

/// A lowered plan together with the arena that owns everything it points at.
///
/// Self-referential by construction — `plan` addresses storage inside `arena` —
/// which is sound because `bumpalo` keeps its chunks on the heap: moving a
/// `CompiledPlan` moves the `Bump` *handle*, never the bytes.
pub struct CompiledPlan {
    /// Owns the nodes, template parts, literals and every interned string. It
    /// is never read — its whole job is to keep `plan`'s storage alive and to
    /// release it on drop, which is what makes a plan reclaimable at all.
    #[allow(dead_code)]
    arena: Bump,
    plan: *const ParserPlan,
}

impl CompiledPlan {
    /// The compiled plan.
    pub fn plan(&self) -> &ParserPlan {
        // SAFETY: `plan` was allocated in `self.arena`, which this borrow keeps
        // alive, and is never mutated after construction.
        unsafe { &*self.plan }
    }
}

impl std::fmt::Debug for CompiledPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.plan().fmt(f)
    }
}

// SAFETY: `CompiledPlan` owns everything it points at, and nothing inside is
// shared with another thread until it is registered behind the arena's mutex.
// The raw pointer is what makes the auto-impl fail; the data is plain,
// immutable, and self-contained.
unsafe impl Send for CompiledPlan {}

/// Lower a validated `ParserAst` into a self-owning [`CompiledPlan`].
///
/// The caller registers it with [`register_plan`] to get the [`PlanId`] MIR
/// embeds.
///
/// # Panics
/// Only on an internal inconsistency (the AST should have passed validation).
pub fn lower_to_plan(ast: &ParserAst, order: &mut dyn FieldOrder) -> CompiledPlan {
    let arena = Bump::new();
    let plan = {
        let mut b = PlanBuilder::new(&arena, order);
        let root = lower_node(&mut b, ast);
        b.finish(root) as *const ParserPlan
    };
    CompiledPlan { arena, plan }
}

/// Who decides the **layout order** of the anonymous record a named-capture
/// parser builds (§5.6, ADR-152).
///
/// An anonymous record's identity is its field-name set, so two parsers that
/// name the same fields in different orders produce one type — and a type has
/// exactly one field order, because a field read compiles to a slot index. The
/// order therefore cannot be a property of the parser that happens to be
/// building the value; it has to come from whatever knows about *every*
/// spelling in the program. During a compile that is the
/// [`TypeDb`](praxis_types::TypeDb), which registered a definition for each.
///
/// The plan stores the answer per record-producing node so the runtime places
/// fields with an index rather than a name lookup.
pub trait FieldOrder {
    /// The canonical order of the shape whose fields are `names`. The answer is
    /// a permutation of `names` — same set, possibly reordered.
    fn canonical(&mut self, names: &[&str]) -> Vec<String>;
}

/// The order the parser wrote them in — [`FieldOrder`] for a plan lowered
/// outside a compilation, where there is no other spelling to agree with.
///
/// Correct on its own terms: with one spelling of a shape, the first one *is*
/// the canonical one. It is what the teardown and plan tests use, and what
/// makes those tests independent of the type arena.
pub struct SourceOrder;

impl FieldOrder for SourceOrder {
    fn canonical(&mut self, names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }
}

impl FieldOrder for praxis_types::TypeDb {
    fn canonical(&mut self, names: &[&str]) -> Vec<String> {
        self.canonical_field_order(names).to_vec()
    }
}

// ===========================================================================
// The plan arena: maps `PlanId`s (passed as an i64 immediate through MIR) to
// their compiled plans. Lives here (not in HIR) so both HIR (which registers)
// and the runtime interpreter (which looks up) depend on this crate without
// creating a dependency cycle.
// ===========================================================================

/// The identity of a registered parser plan.
///
/// `NonZeroU32` on purpose: a `0` meaning "parser analysis failed" would be
/// indistinguishable from the first successfully registered plan. No `u32`
/// means "no plan", so that encoding is unwritable — a failed analysis lowers
/// to an error expression instead.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PlanId(NonZeroU32);

impl PlanId {
    /// The raw id, for embedding as a MIR immediate.
    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// Recover a `PlanId` from a raw value, rejecting `0`.
    ///
    /// The runtime calls this on the integer it reads back out of a boxed `Int`
    /// argument: a value that never named a plan must become a parse fault, not
    /// an index into the arena.
    pub fn from_raw(raw: u32) -> Option<PlanId> {
        NonZeroU32::new(raw).map(PlanId)
    }
}

/// Registration refused: the process has compiled more parser plans than the
/// arena admits.
///
/// This is a compile-time registration bound, not a language limit — one plan
/// per `read`/`parse` expression per compile. Reaching it means something is
/// recompiling in a loop, and saying so beats wrapping the index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TooManyPlans {
    /// The bound that was hit.
    pub limit: usize,
}

impl std::fmt::Display for TooManyPlans {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "too many parser plans registered in one process (limit {})",
            self.limit
        )
    }
}

impl std::error::Error for TooManyPlans {}

/// The registration bound. Chosen so an id always fits a `u32` with room to
/// spare, and so a runaway registration loop is caught long before the
/// narrowing could matter.
pub const MAX_PLANS: usize = 1 << 20;

/// The narrowing `register_plan` performs is checked *and* provable: an id is
/// `len()` after a push, which the bound keeps strictly inside `u32`.
const _: () = assert!(MAX_PLANS < u32::MAX as usize);

/// The process-wide store. Index `i` holds the plan with id `i + 1`, which is
/// what keeps [`PlanId`] non-zero.
static PLAN_ARENA: Mutex<Vec<CompiledPlan>> = Mutex::new(Vec::new());

/// Register a compiled plan, returning its id.
///
/// # Errors
/// [`TooManyPlans`] once [`MAX_PLANS`] plans have been registered.
pub fn register_plan(plan: CompiledPlan) -> Result<PlanId, TooManyPlans> {
    register_with_limit(plan, MAX_PLANS)
}

/// [`register_plan`] with the bound as a parameter, so a test can reach the
/// refusal without registering a million plans.
fn register_with_limit(plan: CompiledPlan, limit: usize) -> Result<PlanId, TooManyPlans> {
    let mut arena = PLAN_ARENA
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if arena.len() >= limit {
        // Refuse *before* pushing, so a rejected registration leaves the arena
        // exactly as it found it.
        return Err(TooManyPlans { limit });
    }
    arena.push(plan);
    let raw = u32::try_from(arena.len()).map_err(|_| TooManyPlans { limit })?;
    Ok(PlanId(
        NonZeroU32::new(raw).expect("len is at least 1 after the push"),
    ))
}

/// Look up a plan by id. `None` if the id names no registered plan (the caller
/// treats that as a parse fault).
pub fn get_plan(id: PlanId) -> Option<&'static ParserPlan> {
    let arena = PLAN_ARENA
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let plan: *const ParserPlan = arena.get(id.get() as usize - 1)?.plan;
    // SAFETY: registered plans are never removed except by `retire_all_plans`, whose
    // caller has proven the heap is drained; the storage lives in the
    // `CompiledPlan`'s arena, on the heap, so the address is stable across the
    // `Vec`'s own reallocations.
    Some(unsafe { &*plan })
}

/// How many plans are registered. For tests and diagnostics.
pub fn plan_count() -> usize {
    PLAN_ARENA
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

/// Drop every registered plan, releasing its arena.
///
/// # Safety
/// Every `&'static str` a plan handed out must be dead. In particular the
/// runtime builds record schemas for named-capture templates whose field names
/// *borrow from plan storage*, and caches them; those caches must be cleared in
/// the same breath. `praxis_runtime::retire_parser_plans` is the only intended
/// caller and does exactly that, behind a `HeapDrained` proof.
pub unsafe fn retire_all_plans() {
    PLAN_ARENA
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Lower one node, returning its index in the plan arena.
fn lower_node(b: &mut PlanBuilder<'_>, ast: &ParserAst) -> u32 {
    /// Lower the one child and wrap it — the entire body of every constructor
    /// whose plan node is `{ child }` and nothing else, and whose plan variant
    /// shares its name with the AST variant.
    ///
    /// The arms written out below are the ones that need more. `Atomic` and
    /// `OneOf` have no child; `Template` has its own function; the rest carry
    /// payload beside the child that has to be allocated or interned *here*, in
    /// the plan's arena — a literal (`Sep`, `OneOf`, `GridRagged`), a skip
    /// policy (`Characters`), or a slice of entries (`SectionsNamed`, `Block`,
    /// `Choice`).
    ///
    /// A macro rather than a helper `fn` because what varies is a *variant
    /// name*, not a value; and invoked in body position rather than expanded
    /// into the arms themselves because macros cannot expand to match arms.
    macro_rules! unary {
        ($node:ident, $child:expr) => {{
            let c = lower_node(b, $child);
            b.push_node(PlanNode::$node { child: c })
        }};
    }

    match ast {
        ParserAst::Atomic { kind, .. } => b.push_node(PlanNode::Atomic { kind: *kind }),
        ParserAst::Lines { child, .. } => unary!(Lines, child),
        ParserAst::Sections { child, .. } => unary!(Sections, child),
        ParserAst::SectionsNamed {
            fields,
            repeated_tail,
            ..
        } => {
            // Lower each named argument's child, keeping source order: a
            // counted group's position among the fields is the position the
            // runtime consumes its sections at.
            let field_entries: Vec<SectionItemNode> = fields
                .iter()
                .map(|item| {
                    let name = b.alloc_str(item.name());
                    let child = lower_node(b, item.parser());
                    match item {
                        crate::ast::SectionItem::One { .. } => SectionItemNode::One { name, child },
                        crate::ast::SectionItem::Counted { count, .. } => {
                            SectionItemNode::Counted {
                                name,
                                child,
                                count: count.get(),
                            }
                        }
                    }
                })
                .collect();
            let tail_entry = repeated_tail.as_ref().map(|(name, p)| {
                let n = b.alloc_str(name);
                let c = lower_node(b, p);
                (n, c)
            });
            // The record's fields are the named arguments in source order, then
            // the unbounded tail, which is the order the runtime assembles them
            // in and therefore the order to ask about.
            let mut names: Vec<&str> = field_entries.iter().map(|f| f.name()).collect();
            if let Some((tail_name, _)) = tail_entry {
                names.push(tail_name);
            }
            let field_order = b.canonical_order(&names);
            let field_slice = b.alloc_slice(field_entries);
            b.push_node(PlanNode::SectionsNamed {
                fields: field_slice,
                repeated_tail: tail_entry,
                field_order,
            })
        }
        ParserAst::Csv { child, .. } => unary!(Csv, child),
        ParserAst::Ws { child, .. } => unary!(Ws, child),
        ParserAst::Sep {
            separator, child, ..
        } => {
            let sep_static: &'static str = b.alloc_str(separator.as_str());
            let sep_idx = b.intern_literal(sep_static);
            let c = lower_node(b, child);
            b.push_node(PlanNode::Sep {
                separator_index: sep_idx,
                child: c,
            })
        }
        ParserAst::Grid { child, .. } => unary!(Grid, child),
        ParserAst::Block { items, .. } => {
            // The block record's fields, in the order the runtime assembles
            // them: a named item contributes its own name, and a positional
            // template contributes each of its captures' names, flattened in
            // place (§7.5). Read off the *AST* because that is where a
            // positional's capture names still are — the lowered child is a
            // node index, and its parts are the runtime's to walk.
            let mut names: Vec<&str> = Vec::new();
            for item in items {
                match item {
                    crate::ast::BlockItem::Positional(p) => names.extend(template_field_names(p)),
                    crate::ast::BlockItem::Named { name, .. } => names.push(name),
                }
            }
            let field_order = b.canonical_order(&names);
            let item_nodes: Vec<BlockItemNode> = items
                .iter()
                .map(|item| match item {
                    crate::ast::BlockItem::Positional(p) => BlockItemNode::Positional {
                        child: lower_node(b, p),
                    },
                    crate::ast::BlockItem::Named { name, parser } => BlockItemNode::Named {
                        name: b.alloc_str(name),
                        child: lower_node(b, parser),
                    },
                })
                .collect();
            let items_slice = b.alloc_slice(item_nodes);
            b.push_node(PlanNode::Block {
                items: items_slice,
                field_order,
            })
        }
        ParserAst::Choice { cases, .. } => {
            let case_entries: Vec<(&'static str, u32)> = cases
                .iter()
                .map(|(name, p)| {
                    let n = b.alloc_str(name);
                    let c = lower_node(b, p);
                    (n, c)
                })
                .collect();
            let cases_slice = b.alloc_slice(case_entries);
            b.push_node(PlanNode::Choice { cases: cases_slice })
        }
        ParserAst::Optional { child, .. } => unary!(Optional, child),
        ParserAst::Scan { child, .. } => unary!(Scan, child),
        ParserAst::OneOf { chars, .. } => {
            let chars_static = b.alloc_str(chars);
            let idx = b.intern_literal(chars_static);
            b.push_node(PlanNode::OneOf { chars_index: idx })
        }
        ParserAst::Characters { child, skip, .. } => {
            let c = lower_node(b, child);
            b.push_node(PlanNode::Characters {
                child: c,
                skip: *skip,
            })
        }
        ParserAst::Matrix { child, .. } => unary!(Matrix, child),
        ParserAst::GridRagged { child, fill, .. } => {
            let c = lower_node(b, child);
            let fill_static = b.alloc_str(fill);
            let fill_idx = b.intern_literal(fill_static);
            b.push_node(PlanNode::GridRagged {
                child: c,
                fill_index: fill_idx,
            })
        }
        ParserAst::Template { parts, .. } => lower_template(b, parts),
    }
}

/// Lower a template into a `PlanNode::Template`.
///
/// **Every shape, one node.** Scalar, tuple and record templates all lower to
/// this, because all three need the literal parts between the captures kept —
/// they are the separators the runtime matches, and they are what makes
/// `` `{int},{int}` `` a pair rather than a comma-less run of digits. Which of
/// the three a given template *is* is [`TemplateShape::of`]'s answer, read from
/// these same parts by the interpreter; this function does not decide it.
/// `captures` is collected only for the field indices [`lower_template_parts`]
/// assigns.
fn lower_template(b: &mut PlanBuilder<'_>, parts: &[TemplatePart]) -> u32 {
    // Capture positions, which is what assigns each capture its field index in
    // the resulting record or tuple.
    let captures: Vec<(usize, &TemplatePart)> = parts
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p, TemplatePart::Capture { .. }))
        .collect();
    // A record shape only — a tuple has no field names to reorder by, and
    // `TemplateShape::of` reads the same "is any capture named?" question off
    // the lowered parts.
    let names = template_field_names_of(parts);
    let field_order = if names.is_empty() {
        &[][..]
    } else {
        b.canonical_order(&names)
    };
    let part_indices = lower_template_parts(b, parts, &captures);
    b.push_node(PlanNode::Template {
        parts: part_indices,
        field_order,
    })
}

/// The field names a *template* parser contributes to a record, in source
/// order, or empty when it builds no record.
///
/// The two callers ask for different reasons — one is lowering the template
/// itself, one is a `block(...)` flattening it (§7.5) — and both need the same
/// answer, which is the one `TemplateShape::Record` is defined by: a template
/// is a record when any capture is named.
fn template_field_names_of(parts: &[TemplatePart]) -> Vec<&str> {
    let names: Vec<&str> = parts
        .iter()
        .filter_map(|p| match p {
            TemplatePart::Capture { name, .. } => name.as_ref().map(|n| n.as_str()),
            TemplatePart::Literal { .. } => None,
        })
        .collect();
    names
}

/// [`template_field_names_of`] for a parser that *may* be a template. A
/// positional `block(...)` item that is not one contributes no fields —
/// validation has already refused it (I026).
fn template_field_names(ast: &ParserAst) -> Vec<&str> {
    match ast {
        ParserAst::Template { parts, .. } => template_field_names_of(parts),
        _ => Vec::new(),
    }
}

/// Lower template parts into the `template_parts` arena, returning a static
/// slice reference. `captures` lets us assign field indices.
fn lower_template_parts(
    b: &mut PlanBuilder<'_>,
    parts: &[TemplatePart],
    captures: &[(usize, &TemplatePart)],
) -> &'static [TemplatePartNode] {
    let mut nodes = Vec::new();
    for part in parts {
        match part {
            TemplatePart::Literal { text, ws, .. } => {
                let text_static = b.alloc_str(text);
                nodes.push(TemplatePartNode::Literal {
                    text: text_static,
                    ws: *ws,
                });
            }
            TemplatePart::Capture { name, parser, .. } => {
                let child = lower_node(b, parser);
                let field_index = captures
                    .iter()
                    .position(|(_, p)| std::ptr::eq(*p, part))
                    .map(|i| i as u16);
                let name_static = name.as_ref().map(|n| b.alloc_str(n.as_str()));
                nodes.push(TemplatePartNode::Capture {
                    child,
                    field_index,
                    name: name_static,
                });
            }
        }
    }
    b.alloc_slice(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AtomicKind, Separator, TemplatePart, WsPolicy};
    use praxis_source::Span;

    #[test]
    fn atomic_lower_to_plan() {
        let ast = ParserAst::Atomic {
            kind: AtomicKind::Int,
            span: Span::at(0),
        };
        let compiled = lower_to_plan(&ast, &mut SourceOrder);
        let plan = compiled.plan();
        assert_eq!(plan.root, 0);
        assert!(matches!(
            plan.nodes[0],
            PlanNode::Atomic {
                kind: AtomicKind::Int
            }
        ));
    }

    #[test]
    fn lines_of_int_lower_to_plan() {
        let ast = ParserAst::Lines {
            child: Box::new(ParserAst::Atomic {
                kind: AtomicKind::Int,
                span: Span::at(0),
            }),
            span: Span::at(0),
        };
        let compiled = lower_to_plan(&ast, &mut SourceOrder);
        let plan = compiled.plan();
        // Children are lowered first (lower index); the parent (Lines) is root.
        assert!(matches!(
            plan.nodes[0],
            PlanNode::Atomic {
                kind: AtomicKind::Int
            }
        ));
        assert!(matches!(
            plan.nodes[plan.root as usize],
            PlanNode::Lines { child: 0 }
        ));
    }

    #[test]
    fn sep_lower_interns_separator() {
        let ast = ParserAst::Sep {
            separator: Separator::new(" -> ").expect("a non-empty separator"),
            child: Box::new(ParserAst::Atomic {
                kind: AtomicKind::Word,
                span: Span::at(0),
            }),
            span: Span::at(0),
        };
        let compiled = lower_to_plan(&ast, &mut SourceOrder);
        let plan = compiled.plan();
        match plan.nodes[plan.root as usize] {
            PlanNode::Sep {
                separator_index, ..
            } => {
                assert_eq!(plan.literals[separator_index as usize], " -> ");
            }
            _ => panic!("expected Sep at root"),
        }
    }

    /// A `PlanId` cannot be zero, so a `0` failure sentinel has no encoding.
    #[test]
    fn zero_is_not_a_plan_id() {
        assert!(PlanId::from_raw(0).is_none());
        assert_eq!(PlanId::from_raw(1).map(PlanId::get), Some(1));
    }

    /// Registration hands out non-zero ids that round-trip through the raw
    /// `u32` MIR embeds, and each one resolves to the plan it named.
    #[test]
    fn registered_plans_round_trip_through_their_raw_id() {
        let first = register_plan(lower_to_plan(
            &ParserAst::Atomic {
                kind: AtomicKind::Int,
                span: Span::at(0),
            },
            &mut SourceOrder,
        ))
        .expect("the arena is far from full");
        let second = register_plan(lower_to_plan(
            &ParserAst::Atomic {
                kind: AtomicKind::Word,
                span: Span::at(0),
            },
            &mut SourceOrder,
        ))
        .expect("the arena is far from full");
        assert_ne!(first, second);
        for (id, expected) in [(first, AtomicKind::Int), (second, AtomicKind::Word)] {
            let raw = id.get();
            assert!(raw > 0, "a plan id is never zero");
            let recovered = PlanId::from_raw(raw).expect("a registered id is non-zero");
            let plan = get_plan(recovered).expect("a registered plan resolves");
            assert!(
                matches!(plan.nodes[plan.root as usize], PlanNode::Atomic { kind } if kind == expected)
            );
        }
    }

    /// Registration is bounded and refuses cleanly: the caller gets a
    /// diagnostic instead of a wrapped index.
    ///
    /// The bound is a parameter here only so the test need not register a
    /// million plans; a limit of zero refuses deterministically regardless of
    /// what else this test binary has registered in parallel.
    #[test]
    fn registration_past_the_bound_is_refused() {
        let atom = || ParserAst::Atomic {
            kind: AtomicKind::Int,
            span: Span::at(0),
        };
        let refused = register_with_limit(lower_to_plan(&atom(), &mut SourceOrder), 0)
            .expect_err("a zero limit admits no plans at all");
        assert_eq!(refused.limit, 0);
        assert!(refused.to_string().contains("too many parser plans"));
        // The refusal happens before the push, so it consumed nothing: an
        // ordinary registration still succeeds and still yields a usable id.
        let accepted = register_plan(lower_to_plan(&atom(), &mut SourceOrder))
            .expect("the real arena has room");
        assert!(get_plan(accepted).is_some());
    }

    /// An id past the end of the arena resolves to nothing rather than
    /// indexing. The runtime turns that `None` into a parse fault.
    #[test]
    fn an_unregistered_id_resolves_to_nothing() {
        let beyond = PlanId::from_raw(u32::MAX).expect("non-zero");
        assert!(get_plan(beyond).is_none());
    }

    /// The plan's storage really is owned: an interned literal survives being
    /// read back out of the arena, and the `CompiledPlan` is what keeps it
    /// alive.
    #[test]
    fn a_compiled_plan_owns_its_interned_strings() {
        let compiled = lower_to_plan(
            &ParserAst::Sep {
                separator: Separator::new(" -> ").expect("a non-empty separator"),
                child: Box::new(ParserAst::Atomic {
                    kind: AtomicKind::Word,
                    span: Span::at(0),
                }),
                span: Span::at(0),
            },
            &mut SourceOrder,
        );
        assert_eq!(compiled.plan().literals, &[" -> "]);
    }

    /// A named-capture template lowers with the record's **canonical** field
    /// order, not its own (§5.6, ADR-152).
    ///
    /// The order is the [`FieldOrder`]'s answer and the plan carries it, which
    /// is how the runtime lays two spellings of one shape out the same way. The
    /// oracle here stands in for the type arena's "the first spelling of this
    /// shape wrote `w` before `h`"; the template writes them the other way.
    #[test]
    fn a_named_template_carries_the_canonical_field_order_and_not_its_own() {
        struct WThenH;
        impl FieldOrder for WThenH {
            fn canonical(&mut self, _names: &[&str]) -> Vec<String> {
                vec!["w".to_string(), "h".to_string()]
            }
        }
        let named = |name: &str| TemplatePart::Capture {
            name: Some(crate::ast::CaptureName::parse(name).expect("a legal name")),
            parser: Box::new(ParserAst::Atomic {
                kind: AtomicKind::Int,
                span: Span::at(0),
            }),
            span: Span::at(0),
            name_span: None,
        };
        let ast = ParserAst::Template {
            parts: vec![
                named("h"),
                TemplatePart::Literal {
                    text: "x".to_string(),
                    ws: WsPolicy::SpaceRun,
                    span: Span::at(0),
                },
                named("w"),
            ],
            span: Span::at(0),
        };
        let compiled = lower_to_plan(&ast, &mut WThenH);
        let plan = compiled.plan();
        let PlanNode::Template { parts, field_order } = &plan.nodes[plan.root as usize] else {
            panic!("a named-capture template lowers to a Template node");
        };
        assert_eq!(TemplateShape::of(parts), TemplateShape::Record);
        assert_eq!(*field_order, &["w", "h"]);

        // A shape nobody wrote first keeps its own order, which is what
        // `SourceOrder` is: the first spelling *is* the canonical one.
        let compiled = lower_to_plan(&ast, &mut SourceOrder);
        let PlanNode::Template { field_order, .. } =
            &compiled.plan().nodes[compiled.plan().root as usize]
        else {
            panic!("a named-capture template lowers to a Template node");
        };
        assert_eq!(*field_order, &["h", "w"]);
    }

    /// A template that builds no record carries no order to disagree about.
    #[test]
    fn a_tuple_template_carries_no_field_order() {
        let anonymous = || TemplatePart::Capture {
            name: None,
            parser: Box::new(ParserAst::Atomic {
                kind: AtomicKind::Int,
                span: Span::at(0),
            }),
            span: Span::at(0),
            name_span: None,
        };
        let ast = ParserAst::Template {
            parts: vec![
                anonymous(),
                TemplatePart::Literal {
                    text: ",".to_string(),
                    ws: WsPolicy::SpaceRun,
                    span: Span::at(0),
                },
                anonymous(),
            ],
            span: Span::at(0),
        };
        let compiled = lower_to_plan(&ast, &mut SourceOrder);
        let PlanNode::Template { field_order, .. } =
            &compiled.plan().nodes[compiled.plan().root as usize]
        else {
            panic!("a template lowers to a Template node");
        };
        assert!(field_order.is_empty(), "a tuple has no fields to order");
    }

    /// Two anonymous captures lower to a `Template` node — **not** to a tuple
    /// node, because there is none and cannot be one (ADR-092).
    ///
    /// This assertion is the standing proof that the tuple shape takes the
    /// `Template` path. The literals between the captures are preserved on that
    /// path, which is exactly what a `Tuple { elements: &[u32] }` would have
    /// nowhere to put; the interpreter reads the shape back off the parts with
    /// [`TemplateShape::of`] and assembles the tuple from the captured values.
    #[test]
    fn template_literal_lower_to_plan() {
        let ast = ParserAst::Template {
            parts: vec![
                TemplatePart::Capture {
                    name: None,
                    parser: Box::new(ParserAst::Atomic {
                        kind: AtomicKind::Int,
                        span: Span::at(0),
                    }),
                    span: Span::at(0),
                    name_span: None,
                },
                TemplatePart::Literal {
                    text: ",".to_string(),
                    ws: WsPolicy::SpaceRun,
                    span: Span::at(0),
                },
                TemplatePart::Capture {
                    name: None,
                    parser: Box::new(ParserAst::Atomic {
                        kind: AtomicKind::Int,
                        span: Span::at(0),
                    }),
                    span: Span::at(0),
                    name_span: None,
                },
            ],
            span: Span::at(0),
        };
        let compiled = lower_to_plan(&ast, &mut SourceOrder);
        let plan = compiled.plan();
        // The root is the last-pushed node.
        let PlanNode::Template { parts, .. } = &plan.nodes[plan.root as usize] else {
            panic!("a two-anonymous-capture template lowers to a Template node");
        };
        // And the shape the interpreter reads back off those parts is the tuple
        // one, which is the half the node kind alone does not say.
        assert_eq!(TemplateShape::of(parts), TemplateShape::Tuple);
    }

    /// **A counted group keeps its position and its count in the plan.** The
    /// runtime walks `fields` in order with one section cursor, so a counted
    /// item lowered out of order — or lowered without its count — would read a
    /// different span of sections than the source named. The plan is the last
    /// place source order still exists.
    #[test]
    fn a_counted_item_keeps_its_count_and_its_position_in_the_plan() {
        use crate::ast::{RepeatCount, SectionItem};

        let ast = ParserAst::SectionsNamed {
            fields: vec![
                SectionItem::Counted {
                    name: "shapes".to_string(),
                    count: RepeatCount::new(6).expect("six sections"),
                    parser: ParserAst::Lines {
                        child: Box::new(ParserAst::Atomic {
                            kind: AtomicKind::Int,
                            span: Span::at(0),
                        }),
                        span: Span::at(0),
                    },
                },
                SectionItem::One {
                    name: "regions".to_string(),
                    parser: ParserAst::Atomic {
                        kind: AtomicKind::Char,
                        span: Span::at(0),
                    },
                },
            ],
            repeated_tail: None,
            span: Span::at(0),
        };
        let compiled = lower_to_plan(&ast, &mut SourceOrder);
        let plan = compiled.plan();
        let PlanNode::SectionsNamed {
            fields,
            repeated_tail,
            ..
        } = &plan.nodes[plan.root as usize]
        else {
            panic!("a named `sections` lowers to a SectionsNamed node");
        };
        assert!(repeated_tail.is_none(), "a counted group is not the tail");
        assert_eq!(fields.len(), 2);
        match &fields[0] {
            SectionItemNode::Counted { name, child, count } => {
                assert_eq!(*name, "shapes");
                assert_eq!(*count, 6);
                assert!(matches!(
                    plan.nodes[*child as usize],
                    PlanNode::Lines { .. }
                ));
            }
            other => panic!("the first field is the counted group, got {other:?}"),
        }
        match &fields[1] {
            SectionItemNode::One { name, .. } => assert_eq!(*name, "regions"),
            other => panic!("the second field follows the counted group, got {other:?}"),
        }
        // Two sections for `regions` to start at is what the count buys, and
        // the runtime reads it from here.
        assert_eq!(fields[0].sections_wanted(), 6);
        assert_eq!(fields[1].sections_wanted(), 1);
    }
}
